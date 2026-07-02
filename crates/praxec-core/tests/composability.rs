//! Tests proving the gateway is composable in the senses required by INIT-5:
//!
//! - **Named capability reuse** — define once, use in proxy.expose AND in a
//!   workflow transition.
//! - **Capability wrapping** — `wraps:` stacks guards/reliability without
//!   modifying the base.
//! - **Multi-file composition** — `include:` deep-merges sibling YAML files;
//!   maps merge, arrays concatenate.
//! - **Cycle detection** — `wraps:` cycles and `include:` cycles fail loud.
//! - **Idempotent resolve** — `resolve(resolve(x)) == resolve(x)`.
//! - **End-to-end** — a resolved config's named capability really does fire
//!   the right executor when invoked through the runtime.

use std::sync::Arc;

use async_trait::async_trait;
use praxec_core::audit::{MemoryAuditSink, NullAuditSink};
use praxec_core::capability::CapabilityRegistry;
use praxec_core::config;
use praxec_core::error::ExecutorError;
use praxec_core::guards::DefaultGuardEvaluator;
use praxec_core::model::{
    ExecuteRequest, ExecuteResult, Principal, StartWorkflow, SubmitTransition,
};
use praxec_core::ports::{Executor, ExecutorRegistry};
use praxec_core::proxy_workflow::{compile_proxy_workflow, DEFAULT_PROXY_WORKFLOW_ID};
use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
use praxec_core::WorkflowRuntime;
use serde_json::{json, Value};

// ---------- helpers --------------------------------------------------------

fn write(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

#[derive(Default)]
struct RecordingExecutor {
    calls: std::sync::Mutex<Vec<Value>>,
}

#[async_trait]
impl Executor for RecordingExecutor {
    async fn execute(&self, req: ExecuteRequest) -> Result<ExecuteResult, ExecutorError> {
        self.calls.lock().unwrap().push(req.executor_config.clone());
        Ok(ExecuteResult {
            output: json!({ "ok": true }),
            evidence: vec![],
            child_workflow_id: None,
            next_transition: None,
            suspend: None,
            telemetry: None,
        })
    }
}

struct OneExecRegistry(Arc<RecordingExecutor>);
impl ExecutorRegistry for OneExecRegistry {
    fn get(&self, _kind: &str) -> Option<Arc<dyn Executor>> {
        Some(self.0.clone())
    }
}

// ---------- (a) named capability reuse ------------------------------------

#[test]
fn named_capability_reused_in_expose_and_workflow() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "github.create_pr": {
                "title": "Create GitHub PR",
                "description": "Open a pull request on GitHub.",
                "tags": ["github", "write"],
                "inputSchema": {
                    "type": "object",
                    "properties": { "title": { "type": "string" } },
                    "required": ["title"]
                },
                "executor": {
                    "kind": "mcp",
                    "connection": "github",
                    "tool": "create_pull_request"
                }
            }
        },
        "proxy": {
            "expose": [
                { "capability": "github.create_pr" }
            ]
        },
        "workflows": {
            "safe_pr": {
                "initialState": "tested",
                "states": {
                    "tested": {
                        "transitions": {
                            "create_pr": {
                                "target": "done",
                                "executor": { "capability": "github.create_pr" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let resolved = config::resolve(raw).unwrap();

    // The capabilities block is gone after resolution.
    assert!(
        resolved.get("capabilities").is_none(),
        "capabilities block should be stripped after resolve"
    );

    // The exposure has been rewritten to inline form, with `name` defaulting
    // to the capability name and the executor inlined.
    let expose = resolved.pointer("/proxy/expose/0").unwrap();
    assert_eq!(expose["name"], "github.create_pr");
    assert_eq!(expose["executor"]["kind"], "mcp");
    assert_eq!(expose["executor"]["tool"], "create_pull_request");
    assert_eq!(expose["title"], "Create GitHub PR");
    assert_eq!(expose["inputSchema"]["required"], json!(["title"]));

    // The transition's executor has been rewritten to the same inline form.
    let tx_exec = resolved
        .pointer("/workflows/safe_pr/states/tested/transitions/create_pr/executor")
        .unwrap();
    assert_eq!(tx_exec["kind"], "mcp");
    assert_eq!(tx_exec["tool"], "create_pull_request");
    assert!(
        tx_exec.get("capability").is_none(),
        "executor `capability:` ref must be replaced, not retained"
    );
}

// ---------- (b) capability wrapping stacks guards + reliability -----------

#[test]
fn wraps_stacks_guards_and_carries_reliability() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "raw.create_pr": {
                "executor": {
                    "kind": "mcp",
                    "connection": "github",
                    "tool": "create_pull_request"
                },
                "guards": [
                    { "kind": "permission", "permission": "github.write" }
                ]
            },
            "safe.create_pr": {
                "wraps": "raw.create_pr",
                "guards": [
                    { "kind": "evidence", "requires": ["tests_passed"] }
                ],
                "reliability": {
                    "retry": { "maxAttempts": 3 }
                }
            }
        },
        "workflows": {
            "demo": {
                "initialState": "tested",
                "states": {
                    "tested": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "executor": { "capability": "safe.create_pr" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let resolved = config::resolve(raw).unwrap();
    let tx = resolved
        .pointer("/workflows/demo/states/tested/transitions/go")
        .unwrap();

    // Executor inherited from the base capability.
    assert_eq!(tx["executor"]["kind"], "mcp");
    assert_eq!(tx["executor"]["tool"], "create_pull_request");

    // Both guards present, in declaration order (base first, wrapper second).
    let guards = tx["guards"].as_array().unwrap();
    assert_eq!(guards.len(), 2);
    assert_eq!(guards[0]["kind"], "permission");
    assert_eq!(guards[0]["permission"], "github.write");
    assert_eq!(guards[1]["kind"], "evidence");
    assert_eq!(guards[1]["requires"], json!(["tests_passed"]));

    // Wrapper-level reliability propagated.
    assert_eq!(tx["reliability"]["retry"]["maxAttempts"], 3);
}

#[test]
fn transition_local_guards_stack_on_top_of_capability_guards() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "safe.action": {
                "executor": { "kind": "noop" },
                "guards": [{ "kind": "permission", "permission": "act" }]
            }
        },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "s",
                                "guards": [{ "kind": "role", "role": "approver" }],
                                "executor": { "capability": "safe.action" }
                            }
                        }
                    }
                }
            }
        }
    });

    let resolved = config::resolve(raw).unwrap();
    let guards = resolved
        .pointer("/workflows/demo/states/s/transitions/go/guards")
        .unwrap()
        .as_array()
        .unwrap();
    // Capability's permission guard first, then the transition's role guard.
    assert_eq!(guards.len(), 2);
    assert_eq!(guards[0]["kind"], "permission");
    assert_eq!(guards[1]["kind"], "role");
}

#[test]
fn wraps_chain_three_levels_deep_aggregates_correctly() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "a": {
                "executor": { "kind": "noop" },
                "guards": [{ "kind": "permission", "permission": "a" }]
            },
            "b": {
                "wraps": "a",
                "guards": [{ "kind": "permission", "permission": "b" }]
            },
            "c": {
                "wraps": "b",
                "guards": [{ "kind": "permission", "permission": "c" }],
                "reliability": { "timeoutMs": 5000 }
            }
        },
        "proxy": {
            "expose": [{ "capability": "c", "as": "c.use" }]
        }
    });

    let resolved = config::resolve(raw).unwrap();
    let exposure = resolved.pointer("/proxy/expose/0").unwrap();
    assert_eq!(exposure["name"], "c.use");
    let guards = exposure["guards"].as_array().unwrap();
    assert_eq!(guards.len(), 3);
    assert_eq!(guards[0]["permission"], "a");
    assert_eq!(guards[1]["permission"], "b");
    assert_eq!(guards[2]["permission"], "c");
    assert_eq!(exposure["reliability"]["timeoutMs"], 5000);
}

#[test]
fn wraps_cycle_is_detected_and_fails_loud() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "a": { "wraps": "b", "executor": { "kind": "noop" } },
            "b": { "wraps": "a", "executor": { "kind": "noop" } }
        }
    });
    let err = config::resolve(raw).unwrap_err();
    assert!(err.to_string().contains("cycle"), "got: {err}");
}

#[test]
fn unknown_capability_reference_fails_loud() {
    let raw = json!({
        "version": "1.0.0",
        "proxy": {
            "expose": [{ "capability": "missing.thing" }]
        }
    });
    let err = config::resolve(raw).unwrap_err();
    assert!(err.to_string().contains("unknown capability"), "got: {err}");
}

// ---------- (c) include / multi-file composition ---------------------------

#[test]
fn include_merges_files_with_map_and_array_semantics() {
    let dir = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "base.connections.yaml",
        r#"
version: "1.0.0"
connections:
  github:
    kind: mcp
    command: github-mcp-server
"#,
    );

    write(
        dir.path(),
        "team.policy.yaml",
        r#"
version: "1.0.0"
audit:
  sink: stderr
proxy:
  expose:
    - name: hello.from_team
      executor: { kind: noop }
"#,
    );

    let main = write(
        dir.path(),
        "main.yaml",
        r#"
version: "1.0.0"
include:
  - base.connections.yaml
  - team.policy.yaml

connections:
  dotnet:
    kind: cli
    command: dotnet

proxy:
  expose:
    - name: hello.from_main
      executor: { kind: noop }
"#,
    );

    let merged = config::load_yaml(&main).unwrap();

    // Maps merge: both connections present.
    assert!(merged.pointer("/connections/github").is_some());
    assert!(merged.pointer("/connections/dotnet").is_some());

    // Arrays concatenate: both proxy.expose entries.
    let names: Vec<&str> = merged
        .pointer("/proxy/expose")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["hello.from_team", "hello.from_main"]);

    // Scalars: the main file is the last writer for `audit.sink`.
    assert_eq!(merged.pointer("/audit/sink").unwrap(), "stderr");
}

#[test]
fn include_main_body_overrides_includes_on_collision() {
    let dir = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "policy.yaml",
        r#"
version: "1.0.0"
audit: { sink: stderr }
"#,
    );

    let main = write(
        dir.path(),
        "main.yaml",
        r#"
version: "1.0.0"
include: [policy.yaml]
audit: { sink: memory }
"#,
    );

    let merged = config::load_yaml(&main).unwrap();
    assert_eq!(merged.pointer("/audit/sink").unwrap(), "memory");
}

#[test]
fn include_cycle_is_rejected() {
    let dir = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "a.yaml",
        r#"
version: "1.0.0"
include: [b.yaml]
"#,
    );
    let b = write(
        dir.path(),
        "b.yaml",
        r#"
version: "1.0.0"
include: [a.yaml]
"#,
    );
    let err = config::load_yaml(&b).unwrap_err();
    assert!(err.to_string().contains("cycle"), "got: {err}");
}

#[test]
fn include_chains_transitively() {
    let dir = tempfile::tempdir().unwrap();

    write(
        dir.path(),
        "leaf.yaml",
        r#"
version: "1.0.0"
connections:
  shared:
    kind: cli
    command: ls
"#,
    );

    write(
        dir.path(),
        "middle.yaml",
        r#"
version: "1.0.0"
include: [leaf.yaml]
audit:
  sink: stderr
"#,
    );

    let main = write(
        dir.path(),
        "root.yaml",
        r#"
version: "1.0.0"
include: [middle.yaml]
"#,
    );

    let merged = config::load_yaml(&main).unwrap();
    assert!(merged.pointer("/connections/shared").is_some());
    assert_eq!(merged.pointer("/audit/sink").unwrap(), "stderr");
}

// ---------- (c.5) resolve_str: compile-time embedding ---------------------

#[test]
fn resolve_str_handles_inline_yaml_for_compile_time_embedding() {
    // Same shape a developer would put in `include_str!("../config.yaml")`.
    let yaml = r#"
version: "1.0.0"
capabilities:
  echo:
    executor: { kind: noop }
proxy:
  expose:
    - { capability: echo, as: hello }
"#;
    let resolved = config::resolve_str(yaml).unwrap();
    assert_eq!(
        resolved.pointer("/proxy/expose/0/name").unwrap(),
        "hello",
        "compile-time embedded YAML should resolve capability refs"
    );
    assert_eq!(
        resolved.pointer("/proxy/expose/0/executor/kind").unwrap(),
        "noop"
    );
    assert!(
        resolved.get("capabilities").is_none(),
        "capabilities block should be stripped"
    );
}

// ---------- (d) idempotence -----------------------------------------------

#[test]
fn resolve_is_idempotent() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "x": { "executor": { "kind": "noop" } }
        },
        "proxy": {
            "expose": [{ "capability": "x" }]
        }
    });
    let once = config::resolve(raw).unwrap();
    let twice = config::resolve(once.clone()).unwrap();
    assert_eq!(once, twice);
}

// ---------- (e) end-to-end: capability ref dispatches to the right executor

#[tokio::test]
async fn capability_ref_in_workflow_actually_dispatches() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "do_thing": {
                "executor": {
                    "kind": "mcp",
                    "connection": "x",
                    "tool": "thingify"
                }
            }
        },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "executor": { "capability": "do_thing" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });

    let resolved = config::resolve(raw).unwrap();
    let recorder = Arc::new(RecordingExecutor::default());

    let definitions = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let executors = Arc::new(OneExecRegistry(recorder.clone()));
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        definitions,
        store,
        executors,
        guards,
        Arc::new(NullAuditSink),
    );

    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let workflow_id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();

    let response = runtime
        .submit(SubmitTransition {
            workflow_id,
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();
    assert_eq!(response["result"]["status"], "succeeded");

    // The recorder saw an `mcp`-kind executor invocation for the `thingify`
    // tool — proof that the capability reference resolved through to the
    // base executor.
    let calls = recorder.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["kind"], "mcp");
    assert_eq!(calls[0]["tool"], "thingify");
}

// ---------- (f) capability registry sees inline + capability-ref exposures

#[test]
fn capability_registry_picks_up_resolved_exposures() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "ping": { "executor": { "kind": "noop" } }
        },
        "proxy": {
            "expose": [
                { "capability": "ping", "as": "ping.alpha", "tags": ["alpha"] },
                { "capability": "ping", "as": "ping.beta",  "tags": ["beta"]  },
                { "name": "raw.thing", "executor": { "kind": "noop" } }
            ]
        }
    });

    let resolved = config::resolve(raw).unwrap();
    let registry = CapabilityRegistry::from_defined(&resolved);

    let ids: Vec<String> = registry.iter().map(|c| c.id.clone()).collect();
    assert!(ids.contains(&"ping.alpha".to_string()));
    assert!(ids.contains(&"ping.beta".to_string()));
    assert!(ids.contains(&"raw.thing".to_string()));
}

// ---------- (g) proxy_default workflow surfaces capability-ref exposures --

#[test]
fn proxy_default_includes_capability_ref_exposures_as_transitions() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "echo": { "executor": { "kind": "noop" } }
        },
        "proxy": {
            "expose": [{ "capability": "echo", "as": "say" }]
        }
    });

    let resolved = config::resolve(raw).unwrap();
    let workflow = compile_proxy_workflow(&resolved).expect("proxy_default");

    assert_eq!(workflow.pointer("/initialState").unwrap(), "ready");
    assert!(workflow.pointer("/states/ready/transitions/say").is_some());

    // Use the constant to keep this test stable if the proxy id ever changes.
    let _ = DEFAULT_PROXY_WORKFLOW_ID;
}

// ---------- (h) deep_merge has the documented semantics --------------------

#[test]
fn deep_merge_concats_arrays_and_overrides_scalars() {
    let a = json!({ "list": [1, 2], "nested": { "keep": "a", "shared": "from_a" } });
    let b = json!({ "list": [3], "nested": { "shared": "from_b", "extra": "yes" } });
    let merged = config::deep_merge(a, b);
    assert_eq!(merged["list"], json!([1, 2, 3]));
    assert_eq!(merged["nested"]["keep"], "a");
    assert_eq!(merged["nested"]["shared"], "from_b");
    assert_eq!(merged["nested"]["extra"], "yes");
}

// ---------- (i) integration with the audit / memory sink ------------------
//
// Composability shouldn't break the audit story: a transition fired through a
// capability ref should still emit the standard taxonomy.

#[tokio::test]
async fn capability_ref_emits_full_audit_trail() {
    let raw = json!({
        "version": "1.0.0",
        "capabilities": {
            "do_thing": { "executor": { "kind": "noop" } }
        },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "executor": { "capability": "do_thing" }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let resolved = config::resolve(raw).unwrap();
    let recorder = Arc::new(RecordingExecutor::default());
    let audit = Arc::new(MemoryAuditSink::new());
    let runtime = WorkflowRuntime::new(
        Arc::new(ConfigDefinitionStore::from_config(&resolved)),
        Arc::new(InMemoryWorkflowStore::new()),
        Arc::new(OneExecRegistry(recorder.clone())),
        Arc::new(DefaultGuardEvaluator::new()),
        audit.clone() as Arc<dyn praxec_core::audit::AuditSink>,
    );
    let started = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: json!({}),
            principal: Principal::anonymous(),
            trace_id: None,
            run_id: None,
            depth: 0,
            parent: None,
        })
        .await
        .unwrap();
    let id = started["workflow"]["id"].as_str().unwrap().to_string();
    let version = started["workflow"]["version"].as_u64().unwrap();
    runtime
        .submit(SubmitTransition {
            workflow_id: id,
            expected_version: version,
            transition: "go".into(),
            arguments: json!({}),
            principal: Principal::anonymous(),
            summary: None,
            trace_id: None,
            run_id: None,
        })
        .await
        .unwrap();

    let types = audit.event_types();
    for required in [
        "workflow.started",
        "transition.requested",
        "executor.started",
        "executor.succeeded",
        "workflow.transitioned",
        "workflow.completed",
    ] {
        assert!(
            types.iter().any(|t| t == required),
            "missing audit event '{required}'; got: {types:?}"
        );
    }
}
