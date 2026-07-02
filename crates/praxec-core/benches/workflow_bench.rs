//! Criterion benchmarks for praxec-core.
//!
//! Run with: cargo bench -p praxec-core
//!
//! These measure the hot path of the workflow runtime — workflow start,
//! submit, and get operations — to track performance over time and
//! catch regressions.

use std::sync::Arc;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, Criterion};

use praxec_core::audit::{AuditSink, MemoryAuditSink};
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::json;

// ---- Test harness -----------------------------------------------------------

struct NoopExecutor;

#[async_trait::async_trait]
impl Executor for NoopExecutor {
    async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        Ok(ExecuteResult {
            output: json!({"ok": true}),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct SingleExecRegistry {
    inner: Arc<NoopExecutor>,
}

impl ExecutorRegistry for SingleExecRegistry {
    fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
        match kind {
            "noop" | "test" | "mcp" | "cli" | "human" => Some(self.inner.clone()),
            _ => None,
        }
    }
}

/// Build a runtime wired with in-memory stores and a noop executor.
/// The workflow definition has one state (`pending`) with a single
/// transition (`approve`) that runs a noop executor and targets `done`.
fn build_runtime() -> (WorkflowRuntime, Arc<MemoryAuditSink>) {
    let config = json!({
        "version": "1.0.0",
        "workflows": {
            "bench": {
                "initialState": "pending",
                "states": {
                    "pending": {
                        "transitions": {
                            "approve": {
                                "target": "done",
                                "executor": { "kind": "noop" }
                            }
                        }
                    },
                    "done": {
                        "terminal": true
                    }
                }
            }
        }
    });

    let definitions = Arc::new(ConfigDefinitionStore::from_config(&config));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executor = Arc::new(NoopExecutor);
    let executors = Arc::new(SingleExecRegistry { inner: executor });
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

/// Start a workflow and return its id and version for use in submit/get
/// benchmarks.
async fn start_workflow(runtime: &WorkflowRuntime) -> (String, u64) {
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "bench".into(),
            input: json!({"key": "value"}),
            principal: Principal::anonymous(),
            ..Default::default()
        })
        .await
        .expect("start should succeed");
    let id = resp["workflow"]["id"]
        .as_str()
        .expect("workflow id")
        .to_string();
    let version = resp["workflow"]["version"]
        .as_u64()
        .expect("workflow version");
    (id, version)
}

// ---- Benchmark groups -------------------------------------------------------

fn bench_start(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (runtime, _audit) = build_runtime();

    c.bench_function("workflow/start", |b| {
        b.to_async(&rt).iter(|| async {
            let resp = runtime
                .start(StartWorkflow {
                    definition_id: "bench".into(),
                    input: json!({"key": "value"}),
                    principal: Principal::anonymous(),
                    ..Default::default()
                })
                .await
                .expect("start should succeed");
            black_box(resp);
        });
    });
}

fn bench_submit(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (runtime, _audit) = build_runtime();

    // Pre-start a workflow so submit benchmarks don't pay start cost.
    let (_id, _version) = rt.block_on(start_workflow(&runtime));

    c.bench_function("workflow/submit", |b| {
        b.to_async(&rt).iter(|| async {
            // Each iteration starts a fresh workflow to avoid stale-version
            // fast-paths skewing results.
            let (id, version) = start_workflow(&runtime).await;
            let resp = runtime
                .submit(SubmitTransition {
                    workflow_id: id,
                    expected_version: version,
                    transition: "approve".into(),
                    arguments: json!({"reason": "benchmark"}),
                    principal: Principal::anonymous(),
                    summary: None,
                    ..Default::default()
                })
                .await
                .expect("submit should succeed");
            black_box(resp);
        });
    });
}

fn bench_get(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let (runtime, _audit) = build_runtime();

    // Pre-start a workflow so get benchmarks don't pay start cost.
    let (id, _version) = rt.block_on(start_workflow(&runtime));

    c.bench_function("workflow/get", |b| {
        b.to_async(&rt).iter(|| async {
            let resp = runtime
                .get(praxec_core::model::GetWorkflow {
                    workflow_id: id.clone(),
                    principal: Principal::anonymous(),
                    ..Default::default()
                })
                .await
                .expect("get should succeed");
            black_box(resp);
        });
    });
}

criterion_group! {
    name = workflow;
    config = Criterion::default().sample_size(100).warm_up_time(std::time::Duration::from_secs(2));
    targets = bench_start, bench_submit, bench_get
}

criterion_main!(workflow);
