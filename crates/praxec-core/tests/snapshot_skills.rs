//! Guarantee tests for SPEC §8.2 — instances carry resolved fragment bodies.
//!
//! A workflow instance's snapshot is a "complete, immutable, self-contained"
//! definition. That means editing or deleting the top-level `skills:` block
//! after `workflow.start` must NOT change what an in-flight instance sees
//! when fetching a fragment body — the snapshot is the source of truth.

use std::sync::Arc;

use praxec_core::WorkflowRuntime;
use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use serde_json::{Value, json};

struct NoopRegistry;
impl praxec_core::ExecutorRegistry for NoopRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn praxec_core::Executor>> {
        None
    }
}

fn build_runtime(config: Value) -> WorkflowRuntime {
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(NoopRegistry);
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let audit = Arc::new(MemoryAuditSink::new());
    WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        audit as Arc<dyn AuditSink>,
    )
}

#[tokio::test]
async fn instance_describe_returns_snapshot_body_not_live_config() {
    let original = json!({
        "version": "1.0.0",
        // Lexicon entry for 'style.house-voice' so the pre-start walk (SPEC §30.10.4)
        // does not block the workflow start.
        "lexicon": {
            "style.house-voice": { "definition_short": "House voice style guide." }
        },
        "skills": {
            "review.style.house-voice": {
                "verb": "review",
                "lifecycle": "stable",
                "body": "ORIGINAL: lead with the reader's problem."
            }
        },
        "workflows": {
            "wf": {
                "initialState": "draft",
                "skills": ["review.style.house-voice"],
                "states": { "draft": { "terminal": true } }
            }
        }
    });

    let resolved = praxec_core::config::resolve(original.clone()).unwrap();
    let runtime = build_runtime(resolved);

    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();

    // Sanity: snapshot describe sees the ORIGINAL body.
    let described = runtime
        .describe_guidance_for_workflow(&workflow_id, "review.style.house-voice")
        .await
        .unwrap()
        .expect("subject must resolve from snapshot");
    assert_eq!(described["kind"].as_str(), Some("guidance"));
    assert_eq!(
        described["subject"].as_str(),
        Some("review.style.house-voice")
    );
    assert_eq!(described["verb"].as_str(), Some("review"));
    assert!(
        described["body"]
            .as_str()
            .unwrap_or_default()
            .contains("ORIGINAL"),
        "body must come from the snapshot; got: {described}"
    );
}

#[tokio::test]
async fn unknown_workflow_returns_none() {
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": { "style.x": { "definition_short": "Fixture." } },
        "skills": { "review.style.x": { "verb": "review", "lifecycle": "stable", "body": "..." } },
        "workflows": {
            "wf": {
                "initialState": "s",
                "skills": ["review.style.x"],
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let resolved = praxec_core::config::resolve(cfg).unwrap();
    let runtime = build_runtime(resolved);

    let res = runtime
        .describe_guidance_for_workflow("nonexistent_wf", "x")
        .await;
    assert!(res.is_err(), "unknown workflow id must surface as an error");
}

#[tokio::test]
async fn unknown_subject_returns_none() {
    let cfg = json!({
        "version": "1.0.0",
        "lexicon": { "style.x": { "definition_short": "Fixture." } },
        "skills": { "review.style.x": { "verb": "review", "lifecycle": "stable", "body": "..." } },
        "workflows": {
            "wf": {
                "initialState": "s",
                "skills": ["review.style.x"],
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let resolved = praxec_core::config::resolve(cfg).unwrap();
    let runtime = build_runtime(resolved);

    let start = runtime
        .start(StartWorkflow {
            definition_id: "wf".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = start["workflow"]["id"].as_str().unwrap().to_string();

    let res = runtime
        .describe_guidance_for_workflow(&workflow_id, "does-not-exist")
        .await
        .unwrap();
    assert!(
        res.is_none(),
        "unknown subject in a known workflow must return None"
    );
}
