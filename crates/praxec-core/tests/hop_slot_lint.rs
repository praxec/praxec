//! FM-7 load lint (Spec A.1 §7) — slot-named context keys are engine-owned.
//!
//! The five HOP slot names (`verify`, `detect`, `scaffold`, `implement`,
//! `lint_format`) name typed, engine-owned blackboard slots. Only a
//! `hop_slot:`-declared transition may produce one (the engine injects the
//! canonical `Out` contract + wires the resolved cap). A *non*-`hop_slot`
//! transition that writes `$.context.<slot>` — via an `output:` mapping key or a
//! `kind: workflow` `use.outputs` LHS — is the FM-7/FM-13 hole: an unvalidated
//! write to a slot-named key. It MUST fail at config load.

use praxec_core::config::resolve;
use serde_json::{Value, json};

/// A one-state flow with a single non-`hop_slot` transition carrying the given
/// transition body.
fn flow_with_transition(body: Value) -> Value {
    json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "s",
                "states": {
                    "s": { "transitions": { "go": body } },
                    "done": { "terminal": true }
                }
            }
        }
    })
}

#[test]
fn non_hop_slot_output_write_to_slot_key_is_a_load_error() {
    // A plain transition whose `output:` maps a slot-named context key.
    let cfg = flow_with_transition(json!({
        "target": "done",
        "actor": "agent",
        "executor": { "kind": "noop" },
        "output": { "verify": "$.arguments.smuggled" }
    }));
    let err = resolve(cfg).expect_err("a non-hop_slot write to $.context.verify must fail load");
    let msg = err.to_string();
    assert!(
        msg.contains("SLOT_KEY_ENGINE_OWNED") && msg.contains("verify"),
        "error must name the FM-7 code and the offending slot: {msg}"
    );
}

#[test]
fn non_hop_slot_use_outputs_to_slot_key_is_a_load_error() {
    // The `use.outputs` LHS form: a plain kind:workflow child projecting into a
    // slot-named context key without a hop_slot declaration.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "initialState": "s",
                "states": {
                    "s": { "transitions": { "go": {
                        "target": "done",
                        "actor": "deterministic",
                        "executor": {
                            "kind": "workflow",
                            "definitionId": "child",
                            "use": { "inputs": {}, "outputs": { "$.context.detect": "d" } }
                        }
                    } } },
                    "done": { "terminal": true }
                }
            },
            "child": {
                "initialState": "ready",
                "states": { "ready": { "terminal": true } },
                "snippet": { "outputs": { "d": { "type": "object" } } }
            }
        }
    });
    let err = resolve(cfg).expect_err("a non-hop_slot use.outputs into $.context.detect must fail");
    let msg = err.to_string();
    assert!(
        msg.contains("SLOT_KEY_ENGINE_OWNED") && msg.contains("detect"),
        "error must name the FM-7 code and the offending slot: {msg}"
    );
}

#[test]
fn hop_slot_transition_writing_the_slot_key_is_clean() {
    // The legitimate producer: a hop_slot: verify transition. The engine wires
    // the cap + synthesizes the `$.context.verify` write; the lint must exempt it.
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "wf": {
                "stack": "generic",
                "initialState": "s",
                "states": {
                    "s": { "transitions": { "go": {
                        "target": "done",
                        "actor": "agent",
                        "hop_slot": "verify"
                    } } },
                    "done": { "terminal": true }
                }
            },
            "cap.verify.generic": {
                "initialState": "ready",
                "states": { "ready": { "terminal": true } },
                "snippet": {
                    "inputs": { "cwd": { "type": "string" } },
                    "outputs": { "verify": { "$ref": "praxec://hop#/$defs/verifyOut" } }
                }
            }
        }
    });
    resolve(cfg).expect("a hop_slot transition writing its own slot key must load clean");
}

#[test]
fn non_hop_slot_write_to_a_non_slot_key_is_clean() {
    // `notes` is not a reserved slot name — an ordinary context write is fine.
    let cfg = flow_with_transition(json!({
        "target": "done",
        "actor": "agent",
        "executor": { "kind": "noop" },
        "output": { "notes": "$.arguments.freeform" }
    }));
    resolve(cfg).expect("a write to a non-slot context key must load clean");
}

/// The resolved slot cap IS the sanctioned typed producer. A `snippet` capability
/// that declares `snippet.outputs.verify: { $ref: …/verifyOut }` and writes
/// `output.verify` is exactly what a `hop_slot: verify` flow resolves to — its
/// write is runtime-validated against verifyOut, so FM-7 must exempt it. (Mirrors
/// the shape of both shipped packs' `cap.verify.<stack>` — the regression that
/// took down a live gateway with an opaque `-32000`.)
#[test]
fn resolved_slot_cap_producing_typed_out_is_clean() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "cap.verify.ts": {
                "initialState": "ready",
                "snippet": {
                    "inputs": { "cwd": { "type": "string" } },
                    "outputs": { "verify": { "$ref": "praxec://hop#/$defs/verifyOut" } }
                },
                "states": {
                    "ready": { "transitions": { "run": {
                        "target": "done",
                        "actor": "deterministic",
                        "executor": { "kind": "script", "subject": "verify.ui.green" },
                        "output": { "verify": "$.output.json" }
                    } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    resolve(cfg).expect("a slot cap declaring the typed snippet.outputs.<slot> must load clean");
}

/// The exemption is tight: an *untyped* `snippet.outputs.verify` (no canonical
/// `verifyOut` `$ref`) does NOT license a `$.context.verify` write — that would
/// reopen the forge hole. It must still fail FM-7.
#[test]
fn untyped_snippet_output_does_not_earn_exemption() {
    let cfg = json!({
        "version": "1.0.0",
        "workflows": {
            "cap.sneaky": {
                "initialState": "ready",
                "snippet": {
                    "inputs": {},
                    "outputs": { "verify": { "type": "object" } }
                },
                "states": {
                    "ready": { "transitions": { "run": {
                        "target": "done",
                        "actor": "deterministic",
                        "executor": { "kind": "noop" },
                        "output": { "verify": "$.arguments.smuggled" }
                    } } },
                    "done": { "terminal": true }
                }
            }
        }
    });
    let err =
        resolve(cfg).expect_err("an untyped snippet.outputs.verify must not earn the exemption");
    assert!(
        err.to_string().contains("SLOT_KEY_ENGINE_OWNED"),
        "untyped slot output must still fail FM-7: {err}"
    );
}
