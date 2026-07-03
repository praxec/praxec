//! Gateway overhead benchmarks for praxec-core.
//!
//! Run with: cargo bench --bench gateway_overhead
//!
//! These measure the cost of core operations — store writes, audit
//! emission — to track overhead and catch regressions.

use std::sync::Arc;

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use praxec_core::audit::{AuditEvent, AuditSink, MemoryAuditSink, NullAuditSink};
use praxec_core::model::WorkflowInstance;
use praxec_core::ports::WorkflowStore;
use praxec_core::store::InMemoryWorkflowStore;
use praxec_core::store_sqlite::SqliteWorkflowStore;
use serde_json::json;

fn bench_in_memory_store(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    c.bench_function("in_memory_create", |b| {
        b.iter_batched(
            || {
                let store = InMemoryWorkflowStore::new();
                let instance = WorkflowInstance {
                    id: format!("wf_{}", black_box(rand::random::<u64>())),
                    definition_id: "test".to_string(),
                    definition_version: "1.0.0".to_string(),
                    definition: json!({"initialState": "running", "states": {}}),
                    state: "running".to_string(),
                    version: 1,
                    input: json!({"key": "value"}),
                    context: json!({"count": 0}),
                    started_at: chrono::Utc::now(),
                    trace_id: None,
                    run_id: None,
                    cancelled_at: None,
                    cancelled_reason: None,
                    depth: 0,
                    parent: None,
                };
                (store, instance)
            },
            |(store, instance)| {
                rt.block_on(store.create(instance)).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_sqlite_store(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let store = SqliteWorkflowStore::open_in_memory().unwrap();

    c.bench_function("sqlite_create", |b| {
        b.iter_batched(
            || WorkflowInstance {
                id: format!("wf_{}", black_box(rand::random::<u64>())),
                definition_id: "test".to_string(),
                definition_version: "1.0.0".to_string(),
                definition: json!({"initialState": "running", "states": {}}),
                state: "running".to_string(),
                version: 1,
                input: json!({"key": "value"}),
                context: json!({"count": 0}),
                started_at: chrono::Utc::now(),
                trace_id: None,
                run_id: None,
                cancelled_at: None,
                cancelled_reason: None,
                depth: 0,
                parent: None,
            },
            |instance| {
                rt.block_on(store.create(instance)).unwrap();
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn bench_audit_emission(c: &mut Criterion) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let null_sink: Arc<dyn AuditSink> = Arc::new(NullAuditSink);
    let mem_sink: Arc<dyn AuditSink> = Arc::new(MemoryAuditSink::new());

    let event = AuditEvent::new("workflow.transitioned")
        .with_workflow("wf_bench")
        .with_payload(json!({"transition": "go", "from": "start", "to": "done"}));

    c.bench_function("audit_null_sink", |b| {
        b.iter(|| {
            rt.block_on(null_sink.record(black_box(event.clone())))
                .unwrap();
        });
    });

    c.bench_function("audit_memory_sink", |b| {
        b.iter(|| {
            rt.block_on(mem_sink.record(black_box(event.clone())))
                .unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_in_memory_store,
    bench_sqlite_store,
    bench_audit_emission
);
criterion_main!(benches);
