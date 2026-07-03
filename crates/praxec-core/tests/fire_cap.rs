//! SPEC §29.6 — `max_fires_per_visit` runtime enforcement tests.
//!
//! Uses the full submit pipeline to verify:
//! - cap allows N fires, rejects the (N+1)th with TRANSITION_FIRE_CAP_EXCEEDED
//! - counter resets when workflow leaves the state
//! - lightweight: true emits workflow.interaction not workflow.transition
//! - purpose: tag propagates into audit payload

use std::sync::Arc;

use praxec_core::ConfigDefinitionStore;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::config::resolve;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{ExecuteRequest, ExecuteResult};
use praxec_core::model::{Principal, StartWorkflow, SubmitTransition};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::InMemoryWorkflowStore;
use serde_json::{Value, json};

struct NoopExec;
#[async_trait::async_trait]
impl Executor for NoopExec {
    async fn execute(&self, _req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct SingleExec(Arc<dyn Executor>);
impl ExecutorRegistry for SingleExec {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

fn agent() -> Principal {
    Principal {
        subject: "agent".into(),
        roles: vec![],
        permissions: vec![],
    }
}

fn human() -> Principal {
    Principal {
        subject: "human@corp".into(),
        roles: vec!["human".into()],
        permissions: vec![],
    }
}

async fn make_runtime(yaml: &str) -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let raw: Value = serde_yaml::from_str(yaml).unwrap();
    let resolved = resolve(raw).expect("config resolves");
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors: Arc<dyn ExecutorRegistry> = Arc::new(SingleExec(Arc::new(NoopExec)));
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit.clone() as Arc<dyn AuditSink>,
    );
    (runtime, audit)
}

// ── max_fires_per_visit enforcement ───────────────────────────────────────

#[tokio::test]
async fn transition_with_max_fires_2_rejects_third_call() {
    let yaml = r#"version: "1.0.0"
workflows:
  demo:
    initialState: working
    states:
      working:
        transitions:
          retry:
            target: working
            max_fires_per_visit: 2
"#;
    let (runtime, _audit) = make_runtime(yaml).await;
    let start = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: agent(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = start
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let mut version = 0u64;

    for i in 0..2 {
        let resp = runtime
            .submit(SubmitTransition {
                workflow_id: wf_id.clone(),
                transition: "retry".into(),
                expected_version: version,
                arguments: json!({}),
                principal: agent(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .unwrap();
        let status = resp.pointer("/result/status").and_then(Value::as_str);
        assert!(
            status == Some("running"),
            "iteration {i} should succeed; got status: {status:?}"
        );
        version = resp
            .pointer("/workflow/version")
            .and_then(Value::as_u64)
            .unwrap();
    }

    // Third call must hit the cap.
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            transition: "retry".into(),
            expected_version: version,
            arguments: json!({}),
            principal: agent(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    let code = resp.pointer("/error/code").and_then(Value::as_str);
    assert_eq!(
        code,
        Some("TRANSITION_FIRE_CAP_EXCEEDED"),
        "third fire must hit cap; got resp: {resp}"
    );
}

#[tokio::test]
async fn fire_counter_resets_on_state_exit() {
    let yaml = r#"version: "1.0.0"
workflows:
  demo:
    initialState: a
    states:
      a:
        transitions:
          loop_a:
            target: a
            max_fires_per_visit: 2
          leave:
            target: b
      b:
        transitions:
          back:
            target: a
      done:
        terminal: true
"#;
    let (runtime, _audit) = make_runtime(yaml).await;
    let start = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: agent(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = start
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();
    let mut version = 0u64;

    // Fire loop_a twice (cap=2).
    for _ in 0..2 {
        let resp = runtime
            .submit(SubmitTransition {
                workflow_id: wf_id.clone(),
                transition: "loop_a".into(),
                expected_version: version,
                arguments: json!({}),
                principal: agent(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .unwrap();
        version = resp
            .pointer("/workflow/version")
            .and_then(Value::as_u64)
            .unwrap();
    }

    // Leave state a → counter should reset
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            transition: "leave".into(),
            expected_version: version,
            arguments: json!({}),
            principal: agent(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    version = resp
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .unwrap();
    assert_eq!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("b")
    );

    // Re-enter state a
    let resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            transition: "back".into(),
            expected_version: version,
            arguments: json!({}),
            principal: agent(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    version = resp
        .pointer("/workflow/version")
        .and_then(Value::as_u64)
        .unwrap();

    // Now we can fire loop_a TWICE more (counter was reset).
    for i in 0..2 {
        let resp = runtime
            .submit(SubmitTransition {
                workflow_id: wf_id.clone(),
                transition: "loop_a".into(),
                expected_version: version,
                arguments: json!({}),
                principal: agent(),
                summary: None,
                trace_id: None,
                run_id: None,
            })
            .await
            .unwrap();
        let status = resp.pointer("/result/status").and_then(Value::as_str);
        assert!(
            status == Some("running"),
            "re-entry fire {i} should succeed (counter reset); got status: {status:?}"
        );
        version = resp
            .pointer("/workflow/version")
            .and_then(Value::as_u64)
            .unwrap();
    }
}

// ── lightweight transition records ────────────────────────────────────────

#[tokio::test]
async fn lightweight_transition_emits_workflow_interaction_not_transition() {
    let yaml = r#"version: "1.0.0"
workflows:
  demo:
    initialState: working
    states:
      working:
        transitions:
          ask:
            target: working
            actor: human
            purpose: ask
            lightweight: true
            max_fires_per_visit: 3
"#;
    let (runtime, audit) = make_runtime(yaml).await;
    let start = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: agent(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = start
        .pointer("/workflow/id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string();

    let _ = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            transition: "ask".into(),
            expected_version: 0,
            arguments: json!({}),
            principal: human(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let events: Vec<_> = audit.snapshot();
    let interaction = events
        .iter()
        .find(|e| e.event_type == "workflow.interaction");
    let transition = events.iter().find(|e| {
        e.event_type == "workflow.transition"
            && e.payload.pointer("/transition").and_then(Value::as_str) == Some("ask")
    });
    assert!(
        interaction.is_some(),
        "expected workflow.interaction event for lightweight transition; events: {:?}",
        events.iter().map(|e| &e.event_type).collect::<Vec<_>>()
    );
    assert!(
        transition.is_none(),
        "lightweight transition must NOT also emit workflow.transition"
    );
    // purpose propagates into the payload
    let purpose = interaction
        .unwrap()
        .payload
        .pointer("/purpose")
        .and_then(Value::as_str);
    assert_eq!(purpose, Some("ask"));
}
