//! Behavioral controls for coarse model work, not tests of model intelligence.
use praxec_core::amplifier;
use serde_json::{Value, json};

fn config() -> Value {
    serde_json::from_str(include_str!(
        "../../../examples/coarse-task-amplifier/gateway.yaml"
    ))
    .unwrap()
}
fn definition() -> Value {
    config()["workflows"]["coarse_task"].clone()
}

#[test]
fn complete_coarse_task_has_a_valid_tool_verified_path() {
    amplifier::validate(&definition()).unwrap();
    let resolved = praxec_core::config::resolve_str(include_str!(
        "../../../examples/coarse-task-amplifier/gateway.yaml"
    ))
    .unwrap();
    let errors: Vec<_> = praxec_core::validate::validate_workflows(&resolved)
        .into_iter()
        .filter(|d| d.is_error())
        .collect();
    assert!(errors.is_empty(), "{errors:?}");
}

#[test]
fn rejects_bypasses_even_when_hidden_behind_guards() {
    for bypass in [
        "direct",
        "branch",
        "timeout",
        "on_enter",
        "fallback",
        "cycle",
        "stale",
        "while",
        "slots",
        "replace_candidate",
        "untrusted",
        "halt",
    ] {
        let mut d = definition();
        match bypass {
            "direct" => d["states"]["generate"]["transitions"]["produce"]["target"] = json!("done"),
            "branch" => {
                d["states"]["generate"]["transitions"]["produce"]["branches"] = json!([
                    {"when":{"kind":"expr","expr":"false"},"target":"done"}
                ])
            }
            "timeout" => d["onTimeout"] = json!({"target":"done"}),
            "on_enter" => d["states"]["done"]["onEnter"] = json!({"executor":{"kind":"agent"}}),
            "fallback" => {
                d["states"]["verify"]["transitions"]["check"]["reliability"] =
                    json!({"fallback":{"executors":[{"kind":"noop"}]}})
            }
            "cycle" => d["states"]["verify"]["transitions"]["check"]["target"] = json!("generate"),
            "while" => {
                d["states"]["generate"]["while"] = json!({"kind":"expr","expr":"true"});
                d["states"]["generate"]["max_iterations"] = json!(10);
            }
            "slots" => d["states"]["verify"]["slots"] = json!({"candidate":{"scope":"state"}}),
            "replace_candidate" => {
                d["states"]["verify"]["transitions"]["check"]["output"]["candidate"] =
                    json!("$.arguments.unchecked")
            }
            "untrusted" => {
                d["states"]["generate"]["transitions"]["produce"]["executor"]["untrusted"] =
                    json!(true)
            }
            "halt" => {
                d["states"]["generate"]["transitions"]["halt_run"] = json!({
                    "actor":"deterministic", "target":"done", "output":{"candidate":"$.arguments.forged"}
                })
            }
            "stale" => {
                d["states"]["verify"]["transitions"]["check"]["target"] = json!("modify");
                d["states"]["modify"] = json!({"transitions":{"change":{
                    "actor":"deterministic","work_kind":"deterministic","target":"done","executor":{"kind":"mcp"}
                }}});
            }
            _ => unreachable!(),
        }
        assert!(amplifier::validate(&d).is_err(), "accepted {bypass} bypass");
    }
}

#[test]
fn refuses_unclassified_or_misclassified_work_and_fabricated_evidence() {
    for (path, value) in [
        ("/blackboard/candidate/type", json!("string")),
        ("/blackboard/verification/type", json!("boolean")),
        (
            "/states/generate/transitions/produce/work_kind",
            json!("deterministic"),
        ),
        (
            "/states/verify/transitions/check/work_kind",
            json!("generative"),
        ),
        (
            "/states/verify/transitions/check/executor/kind",
            json!("agent"),
        ),
        (
            "/states/verify/transitions/check/executor/treatNonZeroAsFailure",
            json!(false),
        ),
        (
            "/states/verify/transitions/check/output/verification",
            json!("$.arguments.evidence"),
        ),
        (
            "/states/verify/transitions/check/executor/args",
            json!(["model says pass"]),
        ),
        (
            "/states/generate/transitions/produce/executor/enforce_input_grounding",
            json!(false),
        ),
        (
            "/states/generate/transitions/produce/executor/goal",
            json!("Do something useful"),
        ),
        (
            "/states/generate/transitions/produce/executor/max_seconds",
            json!(0),
        ),
        (
            "/states/generate/transitions/produce/output/candidate",
            json!("$.arguments.candidate"),
        ),
        (
            "/states/generate/transitions/produce/executor/kind",
            json!("parallel"),
        ),
        ("/states/done/outcome", json!("unknown")),
    ] {
        let mut d = definition();
        *d.pointer_mut(path).unwrap() = value;
        assert!(amplifier::validate(&d).is_err(), "accepted {path}");
    }
}

#[test]
fn bounds_total_model_steps_across_the_entire_task() {
    let mut d = definition();
    d["states"]["revise"] = d["states"]["generate"].clone();
    d["states"]["generate"]["transitions"]["produce"]["target"] = json!("revise");
    assert!(
        amplifier::validate(&d)
            .unwrap_err()
            .to_string()
            .contains("max_model_steps")
    );
    d["amplifier"]["max_model_steps"] = json!(2);
    amplifier::validate(&d).unwrap();
}

#[test]
fn existing_workflows_are_unchanged_without_the_profile() {
    let mut d = definition();
    d.as_object_mut().unwrap().remove("amplifier");
    d["states"]["generate"]["transitions"]["produce"]["target"] = json!("done");
    amplifier::validate(&d).unwrap();
}

mod runtime {
    use super::*;
    use praxec_core::{
        audit::MemoryAuditSink,
        error::ExecutorError,
        guards::DefaultGuardEvaluator,
        model::{ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition},
        ports::{Executor, ExecutorRegistry},
        runtime::WorkflowRuntime,
        store::{ConfigDefinitionStore, InMemoryWorkflowStore},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Worker {
        calls: Arc<AtomicUsize>,
        fail: bool,
        verify: bool,
    }
    #[async_trait::async_trait]
    impl Executor for Worker {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(ExecutorError::Permanent(
                    "test verifier rejected candidate".into(),
                ));
            }
            Ok(ExecuteResult {
                output: if self.verify {
                    json!({"exitCode":0,"success":true,"scriptHash":"trusted-test-check","stdout":"verified"})
                } else {
                    json!({"summary":"candidate","proposal":"design","risks":["needs semantic evaluation"]})
                },
                ..Default::default()
            })
        }
    }
    struct Registry {
        model: Arc<Worker>,
        verifier: Arc<Worker>,
    }
    struct Noop;
    #[async_trait::async_trait]
    impl Executor for Noop {
        async fn execute(&self, _: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
            Ok(ExecuteResult::default())
        }
    }
    impl ExecutorRegistry for Registry {
        fn get(&self, kind: &str) -> Option<Arc<dyn Executor>> {
            match kind {
                "agent" => Some(self.model.clone()),
                "script" => Some(self.verifier.clone()),
                "noop" => Some(Arc::new(Noop)),
                _ => None,
            }
        }
    }
    fn runtime(c: Value, fail: bool) -> (WorkflowRuntime, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let models = Arc::new(AtomicUsize::new(0));
        let checks = Arc::new(AtomicUsize::new(0));
        let registry = Registry {
            model: Arc::new(Worker {
                calls: models.clone(),
                fail: false,
                verify: false,
            }),
            verifier: Arc::new(Worker {
                calls: checks.clone(),
                fail,
                verify: true,
            }),
        };
        (
            WorkflowRuntime::new(
                Arc::new(ConfigDefinitionStore::from_config(&c)),
                Arc::new(InMemoryWorkflowStore::new()),
                Arc::new(registry),
                Arc::new(DefaultGuardEvaluator::new()),
                Arc::new(MemoryAuditSink::new()),
            )
            .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]),
            models,
            checks,
        )
    }
    fn request() -> StartWorkflow {
        StartWorkflow {
            definition_id: "coarse_task".into(),
            input: json!({"intent":"Improve safety", "context":"source", "deliverable":"complete design", "acceptance":"explicit risks"}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        }
    }

    #[tokio::test]
    async fn missing_intent_fails_before_any_model_spend() {
        let (rt, models, checks) = runtime(config(), false);
        for input in [
            json!({}),
            json!({"intent":"  ","context":"x","deliverable":"x","acceptance":"x"}),
        ] {
            let mut r = request();
            r.input = input;
            assert!(
                rt.start(r)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("AMPLIFIER_INPUT_INVALID")
            );
        }
        assert_eq!(models.load(Ordering::SeqCst), 0);
        assert_eq!(checks.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bypassing_config_loader_cannot_bypass_the_contract() {
        let mut c = config();
        c["workflows"]["coarse_task"]["states"]["generate"]["transitions"]["produce"]["target"] =
            json!("done");
        let (rt, models, _) = runtime(c, false);
        assert!(
            rt.start(request())
                .await
                .unwrap_err()
                .to_string()
                .contains("AMPLIFIER_INVALID")
        );
        assert_eq!(models.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn auto_drive_executes_one_declared_worker_without_a_paid_argument_chooser() {
        let mut c = config();
        c["workflows"]["coarse_task"]["enable_halt"] = json!(true);
        let c = praxec_core::config::resolve_str(&c.to_string()).unwrap();
        let (rt, models, checks) = runtime(c, false);
        let rt = rt.with_auto_drive_agents(true, "reasoning", vec![], 10);
        let result = rt.start(request()).await.unwrap();
        assert_eq!(result["workflow"]["state"], "done", "{result}");
        assert_eq!(
            models.load(Ordering::SeqCst),
            1,
            "auto-drive must not synthesize a second model call"
        );
        assert_eq!(checks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn tool_verification_runs_automatically_and_failure_cannot_complete() {
        for fail in [false, true] {
            let (rt, models, checks) = runtime(config(), fail);
            let started = rt.start(request()).await.unwrap();
            let result = rt
                .submit(SubmitTransition {
                    workflow_id: started["workflow"]["id"].as_str().unwrap().into(),
                    expected_version: started["workflow"]["version"].as_u64().unwrap(),
                    transition: "produce".into(),
                    arguments: json!({"verification":{"success":true}}),
                    principal: Principal::anonymous(),
                    summary: None,
                    trace_id: None,
                    run_id: None,
                })
                .await
                .unwrap();
            assert_eq!(models.load(Ordering::SeqCst), 1, "{result}");
            assert_eq!(checks.load(Ordering::SeqCst), 1, "{result}");
            if fail {
                assert_ne!(result["workflow"]["state"], "done", "{result}");
            } else {
                assert_eq!(result["workflow"]["state"], "done", "{result}");
                assert_eq!(
                    result["context"]["verification"]["scriptHash"],
                    "trusted-test-check"
                );
            }
        }
    }
}
