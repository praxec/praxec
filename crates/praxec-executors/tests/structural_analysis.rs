//! SPEC §18 — structural_analysis executor tests. One atomic assertion per
//! rule (positive + negative), plus the self-check invariant that every
//! REQUIRED_RULES rule must fire on a fixture that exercises all of them.

use chrono::Utc;
use praxec_core::model::{ExecuteRequest, WorkflowInstance};
use praxec_core::ports::Executor;
use praxec_executors::{REQUIRED_RULES, StructuralAnalysisExecutor};
use serde_json::{Value, json};

fn instance_stub() -> WorkflowInstance {
    WorkflowInstance {
        id: "wf_stub".into(),
        definition_id: "stub".into(),
        definition_version: "0".into(),
        definition: Value::Null,
        state: "s".into(),
        version: 0,
        input: json!({}),
        context: json!({}),
        started_at: Utc::now(),
        trace_id: None,
        run_id: None,
        cancelled_at: None,
        cancelled_reason: None,
        depth: 0,
        parent: None,
    }
}

fn req_for(definition: Value) -> ExecuteRequest {
    ExecuteRequest {
        workflow: instance_stub(),
        transition: None,
        arguments: json!({ "definition": definition }),
        executor_config: Value::Null,
        idempotency_key: None,
        correlation_id: None,
    }
}

async fn analyze(definition: Value) -> Value {
    StructuralAnalysisExecutor
        .execute(req_for(definition))
        .await
        .expect("structural_analysis executes")
        .output
}

fn issues(out: &Value) -> &Vec<Value> {
    out.get("issues")
        .and_then(Value::as_array)
        .expect("output.issues must be an array")
}

fn has_rule(out: &Value, rule: &str) -> bool {
    issues(out).iter().any(|i| i["rule"].as_str() == Some(rule))
}

// ── Positive: well-formed workflow → no issues ──────────────────────────────

#[tokio::test]
async fn well_formed_workflow_returns_no_issues() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "blackboard": ["count"],
                "states": {
                    "s": {
                        "transitions": {
                            "go": { "target": "done" }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let out = analyze(def).await;
    assert!(issues(&out).is_empty(), "expected no issues; got: {out}");
}

// ── NO_TRANSITIONS ──────────────────────────────────────────────────────────

#[tokio::test]
async fn no_transitions_detected() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let out = analyze(def).await;
    assert!(
        has_rule(&out, "NO_TRANSITIONS"),
        "expected NO_TRANSITIONS; got: {out}"
    );
}

// ── UNDEFINED_TARGET ────────────────────────────────────────────────────────

#[tokio::test]
async fn undefined_target_detected() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": { "target": "nonexistent" }
                        }
                    }
                }
            }
        }
    });
    let out = analyze(def).await;
    assert!(has_rule(&out, "UNDEFINED_TARGET"));
}

// ── DEAD_STATE ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn dead_state_detected() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": { "transitions": { "go": { "target": "s" } } },
                    "orphan": { "terminal": true }
                }
            }
        }
    });
    let out = analyze(def).await;
    assert!(has_rule(&out, "DEAD_STATE"));
}

#[tokio::test]
async fn initial_state_not_flagged_as_dead() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": { "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let out = analyze(def).await;
    let dead: Vec<_> = issues(&out)
        .iter()
        .filter(|i| i["rule"].as_str() == Some("DEAD_STATE"))
        .collect();
    assert!(
        dead.is_empty(),
        "initial state must not be flagged; got: {dead:?}"
    );
}

// ── CYCLE_DETECTED (unguarded only) ─────────────────────────────────────────

#[tokio::test]
async fn unguarded_cycle_detected() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "a",
                "states": {
                    "a": { "transitions": { "loop": { "target": "a" } } }
                }
            }
        }
    });
    let out = analyze(def).await;
    assert!(has_rule(&out, "CYCLE_DETECTED"));
}

#[tokio::test]
async fn guarded_cycle_not_flagged() {
    // Self-loop WITH a guard — legitimate pattern (s01 stress test).
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "a",
                "blackboard": ["count"],
                "states": {
                    "a": {
                        "transitions": {
                            "loop": {
                                "target": "a",
                                "guards": [{ "kind": "expr", "expr": "$.context.count < 3" }]
                            }
                        }
                    }
                }
            }
        }
    });
    let out = analyze(def).await;
    let cycles: Vec<_> = issues(&out)
        .iter()
        .filter(|i| i["rule"].as_str() == Some("CYCLE_DETECTED"))
        .collect();
    assert!(
        cycles.is_empty(),
        "guarded cycle must not flag; got: {cycles:?}"
    );
}

// ── UNDECLARED_SLOT_READ ────────────────────────────────────────────────────

#[tokio::test]
async fn undeclared_slot_read_detected() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "blackboard": ["declared"],
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "s",
                                "guards": [{ "kind": "expr", "expr": "$.context.missing > 0" }]
                            }
                        }
                    }
                }
            }
        }
    });
    let out = analyze(def).await;
    assert!(has_rule(&out, "UNDECLARED_SLOT_READ"));
}

#[tokio::test]
async fn declared_slot_read_passes() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "blackboard": ["declared"],
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "s",
                                "guards": [{ "kind": "expr", "expr": "$.context.declared > 0" }]
                            }
                        }
                    }
                }
            }
        }
    });
    let out = analyze(def).await;
    let undeclared: Vec<_> = issues(&out)
        .iter()
        .filter(|i| i["rule"].as_str() == Some("UNDECLARED_SLOT_READ"))
        .collect();
    assert!(
        undeclared.is_empty(),
        "declared slot must pass; got: {undeclared:?}"
    );
}

// ── UNBLESSED_SUBJECT_ROOT ──────────────────────────────────────────────────

#[tokio::test]
async fn unblessed_subject_root_warning() {
    let def = json!({
        "skills": {
            "nonsense.foo.bar": { "verb": "review", "lifecycle": "stable", "body": "x" }
        }
    });
    let out = analyze(def).await;
    assert!(has_rule(&out, "UNBLESSED_SUBJECT_ROOT"));
    let item = issues(&out)
        .iter()
        .find(|i| i["rule"].as_str() == Some("UNBLESSED_SUBJECT_ROOT"))
        .unwrap();
    assert_eq!(item["severity"].as_str(), Some("warning"));
}

#[tokio::test]
async fn blessed_subject_root_no_warning() {
    let def = json!({
        "skills": {
            "review.style.x": { "verb": "review", "lifecycle": "stable", "body": "x" }
        }
    });
    let out = analyze(def).await;
    let warns: Vec<_> = issues(&out)
        .iter()
        .filter(|i| i["rule"].as_str() == Some("UNBLESSED_SUBJECT_ROOT"))
        .collect();
    assert!(warns.is_empty());
}

// ── OVERSIZED_STATE ─────────────────────────────────────────────────────────

#[tokio::test]
async fn oversized_state_warning_over_threshold() {
    let mut transitions = serde_json::Map::new();
    for i in 0..10 {
        // Each transition gets a guard so we don't trip CYCLE_DETECTED.
        transitions.insert(
            format!("t{i}"),
            json!({
                "target": "s",
                "guards": [{ "kind": "expr", "expr": "$.context.x > 0" }]
            }),
        );
    }
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "blackboard": ["x"],
                "states": {
                    "s": { "transitions": transitions }
                }
            }
        }
    });
    let out = analyze(def).await;
    assert!(has_rule(&out, "OVERSIZED_STATE"));
}

#[tokio::test]
async fn small_state_no_oversized_warning() {
    let def = json!({
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": { "transitions": { "go": { "target": "done" } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let out = analyze(def).await;
    let oversized: Vec<_> = issues(&out)
        .iter()
        .filter(|i| i["rule"].as_str() == Some("OVERSIZED_STATE"))
        .collect();
    assert!(oversized.is_empty());
}

// ── Meta: rules-self-check (SPEC §18.3 + FMECA FM-5) ────────────────────────
//
// A fixture that triggers every required rule must produce issues for
// every required rule. If a future implementer ships an executor with only
// some rules, this test fails with a precise list of missing rules.

#[tokio::test]
async fn every_required_rule_fires_on_kitchen_sink_fixture() {
    let def = json!({
        // Hits UNBLESSED_SUBJECT_ROOT
        "skills": {
            "nonsense.foo.bar": { "verb": "review", "lifecycle": "stable", "body": "x" }
        },
        "workflows": {
            // First workflow: no transitions → NO_TRANSITIONS
            "no_trans": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            },
            // Second workflow: hits the remaining rules
            "kitchen_sink": {
                "initialState": "start",
                "blackboard": ["declared"],
                "states": {
                    "start": {
                        "transitions": {
                            // target → UNDEFINED_TARGET; inline cli command → UNTRUSTED_RAW_EXECUTION
                            "to_missing": { "target": "nowhere", "executor": { "kind": "cli", "command": "rm" } },
                            "loop_unguarded": { "target": "start" },
                            "read_missing": {
                                "target": "start",
                                "guards": [{ "kind": "expr", "expr": "$.context.undeclared > 0" }]
                            },
                            // Pad with guarded transitions to trigger OVERSIZED_STATE
                            "t1": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 0"}]},
                            "t2": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 1"}]},
                            "t3": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 2"}]},
                            "t4": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 3"}]},
                            "t5": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 4"}]},
                            "t6": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 5"}]},
                            "t7": { "target": "start", "guards": [{"kind":"expr","expr":"$.context.declared > 6"}]}
                        }
                    },
                    // Dead state (no inbound, not initial)
                    "orphaned": { "terminal": true }
                }
            }
        }
    });
    let out = analyze(def).await;
    let fired_rules: std::collections::HashSet<&str> = issues(&out)
        .iter()
        .filter_map(|i| i["rule"].as_str())
        .collect();
    let mut missing: Vec<&'static str> = REQUIRED_RULES
        .iter()
        .filter(|r| !fired_rules.contains(*r))
        .copied()
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "self-check failed — required rules did not fire on kitchen-sink fixture: {:?}\n\
         got issues: {:?}",
        missing,
        fired_rules
    );
}

// ── Negative: missing `definition` argument fails fast ──────────────────────

#[tokio::test]
async fn missing_definition_argument_errors() {
    let req = ExecuteRequest {
        workflow: instance_stub(),
        transition: None,
        arguments: json!({}),
        executor_config: Value::Null,
        idempotency_key: None,
        correlation_id: None,
    };
    let err = StructuralAnalysisExecutor
        .execute(req)
        .await
        .expect_err("missing definition must error");
    assert!(format!("{err:?}").contains("definition"));
}
