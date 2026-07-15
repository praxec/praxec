//! End-to-end for #7 — the `path_grounding` gate blocks a flow from advancing
//! past a sign-off when the plan references a path that does not exist under the
//! run's repo_root, and lets it through when every referenced path is real.
//!
//! This pins the "invented path" hallucination class (a plan naming
//! `src/sysadmin/.../CatalogView.tsx` when the real tree is
//! `src/_components/tools/SysadminSignalCatalog/`): the deterministic gate fires
//! on `start`, fails closed (`CHAIN_FAILED`), and the instance never reaches
//! `awaiting_signoff`.

use std::sync::Arc;

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{Principal, StartWorkflow};
use praxec_core::ports::{Executor, ExecutorRegistry, WorkflowStore};
use praxec_core::runtime::WorkflowRuntime;
use praxec_core::store::{ConfigDefinitionStore, InMemoryEvidenceStore, InMemoryWorkflowStore};
use praxec_core::{RepoRoot, RunEnv};
use praxec_executors::PathGroundingExecutor;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Registry mapping the `path_grounding` kind to the real executor; the flow has
/// no other executor-backed transition.
struct GroundingOnlyRegistry {
    exec: Arc<dyn Executor>,
}
impl ExecutorRegistry for GroundingOnlyRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        (kind == "path_grounding").then(|| self.exec.clone())
    }
}

/// A flow whose single deterministic `ground` transition runs `path_grounding`
/// against `$.context.draft_plan.files`, then targets `awaiting_signoff`.
fn config() -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "flow.add-feature": {
                "initialState": "grounding_plan",
                "states": {
                    "grounding_plan": {
                        "transitions": {
                            "ground": {
                                "target": "awaiting_signoff",
                                "actor": "deterministic",
                                "executor": {
                                    "kind": "path_grounding",
                                    "groundedPaths": ["$.context.draft_plan.files"]
                                }
                            }
                        }
                    },
                    "awaiting_signoff": { "terminal": true }
                }
            }
        }
    })
}

fn runtime() -> WorkflowRuntime {
    let cfg = config();
    let audit = Arc::new(MemoryAuditSink::new());
    let definitions = Arc::new(ConfigDefinitionStore::from_config(&cfg));
    let store: Arc<dyn WorkflowStore> = Arc::new(InMemoryWorkflowStore::new());
    let evidence = Arc::new(InMemoryEvidenceStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::with_evidence(evidence.clone()));
    let registry = Arc::new(GroundingOnlyRegistry {
        exec: Arc::new(PathGroundingExecutor::new()),
    }) as Arc<dyn ExecutorRegistry>;
    WorkflowRuntime::new(
        definitions,
        store,
        registry,
        guards,
        audit as Arc<dyn AuditSink>,
    )
    .with_evidence(evidence)
}

/// A repo tree with the real component dir + a plan naming the INVENTED path.
fn repo() -> TempDir {
    let td = TempDir::new().unwrap();
    let real = td
        .path()
        .join("src/_components/tools/SysadminSignalCatalog");
    std::fs::create_dir_all(&real).unwrap();
    std::fs::write(real.join("View.tsx"), b"// real\n").unwrap();
    td
}

#[tokio::test]
async fn invented_path_blocks_advance_past_signoff() {
    let td = repo();
    let rt = runtime();
    // The plan references a real file AND an invented one.
    let resp = rt
        .start(StartWorkflow {
            definition_id: "flow.add-feature".into(),
            input: json!({
                "draft_plan": {
                    "files": [
                        "src/_components/tools/SysadminSignalCatalog/View.tsx",
                        "src/sysadmin/signal-catalog/components/CatalogView.tsx"
                    ]
                }
            }),
            principal: Principal::anonymous(),
            run_env: RunEnv::new(RepoRoot::new(td.path()).unwrap(), None, None),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response");

    // The gate fires on start and fails closed: the flow does NOT advance to
    // `awaiting_signoff`, and the failure names the invented path.
    let status = resp
        .pointer("/result/status")
        .and_then(Value::as_str)
        .unwrap_or("?");
    assert_eq!(status, "failed", "gate must fail closed: {resp:#}");
    assert_ne!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("awaiting_signoff"),
        "must never advance past the gate on an invented path: {resp:#}"
    );
    assert_eq!(
        resp.pointer("/error/code").and_then(Value::as_str),
        Some("CHAIN_FAILED"),
        "resp: {resp:#}"
    );
    let message = resp
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        message.contains("PATH_NOT_GROUNDED")
            && message.contains("src/sysadmin/signal-catalog/components/CatalogView.tsx"),
        "failure must name the invented path: {resp:#}"
    );
}

#[tokio::test]
async fn all_real_paths_advance_to_signoff() {
    let td = repo();
    let rt = runtime();
    let resp = rt
        .start(StartWorkflow {
            definition_id: "flow.add-feature".into(),
            input: json!({
                "draft_plan": {
                    "files": ["src/_components/tools/SysadminSignalCatalog/View.tsx"]
                }
            }),
            principal: Principal::anonymous(),
            run_env: RunEnv::new(RepoRoot::new(td.path()).unwrap(), None, None),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start returns a response");

    // NOTE: the flow seeds `draft_plan` from input into context via the runtime's
    // initialContext convention; if grounded, the flow reaches `awaiting_signoff`.
    assert_eq!(
        resp.pointer("/workflow/state").and_then(Value::as_str),
        Some("awaiting_signoff"),
        "a fully-grounded plan advances past the gate: {resp:#}"
    );
}
