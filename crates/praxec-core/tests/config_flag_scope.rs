//! SPEC §8.4 + §20.2 — `praxec.*` flags are runtime-only and MUST be
//! rejected when nested inside any `workflows:` definition. Otherwise an
//! LLM-authored workflow could embed a key intending to flip the bypass
//! flag on for itself.

use praxec_core::config;
use serde_json::{Value, json};

// ── Top-level praxec.* is fine ────────────────────────────────────────────

#[test]
fn top_level_praxec_block_accepted() {
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "authoring": { "write_enabled": true } },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    config::resolve(cfg).expect("top-level praxec block must be accepted");
}

#[test]
fn top_level_praxec_strict_namespacing_accepted() {
    let cfg = json!({
        "version": "1.0.0",
        "praxec": { "strict_namespacing": false },
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    config::resolve(cfg).expect("top-level strict_namespacing must be accepted");
}

// ── Nested under workflows.* → reject ───────────────────────────────────────

#[test]
fn praxec_object_inside_workflow_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "praxec": { "authoring": { "write_enabled": true } },
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("must reject");
    let msg = format!("{err}");
    assert!(
        msg.contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"),
        "expected CONFIG_FLAG_NOT_RUNTIME_MUTABLE in error; got: {msg}"
    );
    assert!(
        msg.contains("praxec"),
        "error must name the offending key; got: {msg}"
    );
}

#[test]
fn praxec_dot_key_inside_workflow_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "praxec.authoring.write_enabled": true,
                "states": { "s": { "terminal": true } }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("must reject dotted form too");
    assert!(format!("{err}").contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"));
}

#[test]
fn praxec_nested_under_state_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "terminal": true,
                        "praxec": { "strict_namespacing": false }
                    }
                }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("nested under state must reject");
    let msg = format!("{err}");
    assert!(msg.contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"));
    // Path must reference the state where the flag was found.
    assert!(
        msg.contains("/workflows/demo/states/s"),
        "error must include JSON-Pointer path naming the location; got: {msg}"
    );
}

#[test]
fn praxec_nested_deep_inside_transition_rejected() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "transitions": {
                            "go": {
                                "target": "done",
                                "praxec": { "authoring": { "write_enabled": true } }
                            }
                        }
                    },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let err = config::resolve(cfg).expect_err("must reject in transition");
    let msg = format!("{err}");
    assert!(msg.contains("CONFIG_FLAG_NOT_RUNTIME_MUTABLE"));
    assert!(msg.contains("/workflows/demo/states/s/transitions/go"));
}

// ── Confounders: legitimate keys that contain "praxec" elsewhere ──────────

#[test]
fn workflow_id_named_praxec_accepted() {
    // The validator scans for `praxec` as an OBJECT KEY inside a
    // workflow def, not as a workflow id. A workflow literally named
    // "praxec" should still load.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "praxec": {
                "initialState": "s",
                "states": { "s": { "terminal": true } }
            }
        }
    });
    config::resolve(cfg).expect("workflow id 'praxec' must be accepted");
}

#[test]
fn unrelated_key_containing_praxec_substring_accepted() {
    // `praxec-style` is not a praxec.* runtime flag — just a label
    // someone might use.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "demo": {
                "initialState": "s",
                "states": {
                    "s": {
                        "terminal": true,
                        "praxec-style-marker": "ok"
                    }
                }
            }
        }
    });
    config::resolve(cfg).expect("unrelated 'praxec-...' key must be accepted (no dot)");
}

// ── Edge: empty workflows block + no praxec keys → no-op ──────────────────

#[test]
fn no_workflows_block_at_all_accepted() {
    let cfg: Value = json!({
        "version": "1.0.0",
        "proxy": { "expose": [{ "name": "hello", "executor": { "kind": "noop" } }] }
    });
    config::resolve(cfg).expect("no workflows: block, nothing to validate");
}
