//! Integration tests for multi-tenant identity and authorization.
//!
//! Tests prove that different principals see different link surfaces
//! from the same workflow state based on their roles and permissions.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::WorkflowRuntime;
use praxec_core::audit::MemoryAuditSink;
use praxec_core::config::resolve_str;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use serde_json::json;

fn config_yaml() -> &'static str {
    r#"
version: "1.0.0"
workflows:
  tenant_demo:
    initialState: open
    states:
      open:
        linkFilter: byGuards
        transitions:
          admin_action:
            target: done
            guards:
              - kind: role
                role: admin
          user_action:
            target: done
            guards:
              - kind: role
                role: user
          write_action:
            target: done
            guards:
              - kind: permission
                permission: write
      done:
        terminal: true
  all_of_demo:
    initialState: open
    states:
      open:
        linkFilter: byGuards
        transitions:
          admin_write:
            target: done
            guards:
              - kind: all_of
                guards:
                  - kind: role
                    role: admin
                  - kind: permission
                    permission: write
      done:
        terminal: true
"#
}

fn build_runtime() -> WorkflowRuntime {
    let config = resolve_str(config_yaml()).unwrap();
    let audit: Arc<praxec_core::audit::MemoryAuditSink> = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store: Arc<dyn praxec_core::ports::WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));

    // Minimal executor registry with noop
    struct NoopExecutor;
    #[async_trait]
    impl Executor for NoopExecutor {
        async fn execute(&self, _request: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    struct SingleExecutorRegistry(Arc<dyn Executor>);
    impl ExecutorRegistry for SingleExecutorRegistry {
        fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
            Some(self.0.clone())
        }
    }

    WorkflowRuntime::new(
        definitions,
        store,
        Arc::new(SingleExecutorRegistry(Arc::new(NoopExecutor))),
        guards,
        audit,
    )
    .with_evidence(evidence)
}

fn admin() -> Principal {
    Principal {
        subject: "admin@corp.com".to_string(),
        roles: vec!["admin".to_string()],
        permissions: vec!["write".to_string(), "read".to_string()],
    }
}

fn user() -> Principal {
    Principal {
        subject: "user@corp.com".to_string(),
        roles: vec!["user".to_string()],
        permissions: vec!["read".to_string()],
    }
}

fn writer() -> Principal {
    Principal {
        subject: "writer@corp.com".to_string(),
        roles: vec![],
        permissions: vec!["write".to_string()],
    }
}

fn get_links(response: &serde_json::Value) -> Vec<String> {
    response
        .get("links")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("rel").and_then(|v| v.as_str()))
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn admin_sees_admin_links() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: admin(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let links = get_links(&resp);
    assert!(
        links.contains(&"admin_action".to_string()),
        "admin should see admin_action link, got: {:?}",
        links
    );
}

#[tokio::test]
async fn admin_does_not_see_user_links() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: admin(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let links = get_links(&resp);
    assert!(
        !links.contains(&"user_action".to_string()),
        "admin should NOT see user_action link, got: {:?}",
        links
    );
}

#[tokio::test]
async fn user_sees_user_links() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: user(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let links = get_links(&resp);
    assert!(
        links.contains(&"user_action".to_string()),
        "user should see user_action link, got: {:?}",
        links
    );
}

#[tokio::test]
async fn user_does_not_see_admin_links() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: user(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let links = get_links(&resp);
    assert!(
        !links.contains(&"admin_action".to_string()),
        "user should NOT see admin_action link, got: {:?}",
        links
    );
}

#[tokio::test]
async fn writer_sees_write_action() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: writer(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let links = get_links(&resp);
    assert!(
        links.contains(&"write_action".to_string()),
        "writer should see write_action link, got: {:?}",
        links
    );
}

#[tokio::test]
async fn anonymous_sees_no_links() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let links = get_links(&resp);
    assert!(
        links.is_empty(),
        "anonymous should see zero links, got: {:?}",
        links
    );
}

#[tokio::test]
async fn admin_can_submit_admin_action() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: admin(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = resp["workflow"]["id"].as_str().unwrap().to_string();
    let version = resp["workflow"]["version"].as_u64().unwrap();

    let submit_resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            expected_version: version,
            transition: "admin_action".to_string(),
            arguments: json!({}),
            principal: admin(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        submit_resp["workflow"]["state"], "done",
        "admin should transition to done state via admin_action: {submit_resp}"
    );
}

#[tokio::test]
async fn user_cannot_submit_admin_action() {
    let runtime = build_runtime();
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: admin(), // Start as admin so the link exists
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let wf_id = resp["workflow"]["id"].as_str().unwrap().to_string();
    let version = resp["workflow"]["version"].as_u64().unwrap();

    let submit_resp = runtime
        .submit(SubmitTransition {
            workflow_id: wf_id.clone(),
            expected_version: version,
            transition: "admin_action".to_string(),
            arguments: json!({}),
            principal: user(), // Submit as user -> should fail
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        submit_resp["error"]["code"], "GUARD_REJECTED",
        "user should be rejected from admin_action: {submit_resp}"
    );
}

#[tokio::test]
async fn link_filter_by_guards_respects_principal() {
    let runtime = build_runtime();

    // Admin sees admin_action
    let resp_admin = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: admin(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let admin_links = get_links(&resp_admin);

    // User sees user_action
    let resp_user = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: user(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let user_links = get_links(&resp_user);

    // Writer sees write_action
    let resp_writer = runtime
        .start(StartWorkflow {
            definition_id: "tenant_demo".to_string(),
            input: json!({}),
            principal: writer(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let writer_links = get_links(&resp_writer);

    assert!(
        admin_links.contains(&"admin_action".to_string()),
        "admin sees admin_action"
    );
    assert!(
        admin_links.contains(&"write_action".to_string()),
        "admin sees write_action (has write permission)"
    );
    assert!(
        user_links.contains(&"user_action".to_string()),
        "user sees user_action"
    );
    assert!(
        writer_links.contains(&"write_action".to_string()),
        "writer sees write_action"
    );

    assert!(
        !admin_links.contains(&"user_action".to_string()),
        "admin doesn't see user_action"
    );
    assert!(
        !user_links.contains(&"admin_action".to_string()),
        "user doesn't see admin_action"
    );
    assert!(
        !user_links.contains(&"write_action".to_string()),
        "user doesn't see write_action"
    );
}

#[tokio::test]
async fn all_of_guard_respects_principal() {
    let runtime = build_runtime();

    // Admin with write permission sees admin_write
    let resp_admin = runtime
        .start(StartWorkflow {
            definition_id: "all_of_demo".to_string(),
            input: json!({}),
            principal: admin(), // admin role + write permission
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let admin_links = get_links(&resp_admin);
    assert!(
        admin_links.contains(&"admin_write".to_string()),
        "admin with write should see admin_write, got: {:?}",
        admin_links
    );

    // User without admin role or write permission does NOT see admin_write
    let resp_user = runtime
        .start(StartWorkflow {
            definition_id: "all_of_demo".to_string(),
            input: json!({}),
            principal: user(), // user role, read permission only
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let user_links = get_links(&resp_user);
    assert!(
        !user_links.contains(&"admin_write".to_string()),
        "user should NOT see admin_write, got: {:?}",
        user_links
    );
}
