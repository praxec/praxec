//! Full project→framework→language→generic resolution chain for `hop_slot:`
//! (Spec A §5.1). The workflow `stack:` field accepts either a plain string
//! (language-only, back-compat) or an object
//! `{ language, frameworks: [set], primary_framework, project }`. Resolution
//! walks most-specific-first — `cap.<slot>.<project>` → `cap.<slot>.<primary_framework>`
//! → `cap.<slot>.<language>` → `cap.<slot>.generic` — first match wins.

use praxec_core::config::resolve;
use serde_json::{Map, Value, json};

fn verify_cap() -> Value {
    json!({
        "initialState": "ready",
        "states": { "ready": { "terminal": true } },
        "snippet": {
            "inputs": { "cwd": { "type": "string" } },
            "outputs": { "verify": { "$ref": "praxec://hop#/$defs/verifyOut" } }
        }
    })
}

/// Build a host config: a flow with one `hop_slot: verify` transition and the
/// given `stack:` value, plus the named caps.
fn config_with_stack(stack: Value, caps: &[&str]) -> Value {
    let sdlc = json!({
        "stack": stack,
        "initialState": "gate",
        "states": {
            "gate": { "transitions": { "run": {
                "target": "done",
                "actor": "agent",
                "hop_slot": "verify"
            } } },
            "done": { "terminal": true }
        }
    });
    let mut workflows = Map::new();
    workflows.insert("sdlc".into(), sdlc);
    for id in caps {
        workflows.insert((*id).to_string(), verify_cap());
    }
    json!({ "version": "1.0.0", "workflows": Value::Object(workflows) })
}

fn resolved_cap(config: Value) -> String {
    let resolved = resolve(config).expect("resolves");
    resolved
        .pointer("/workflows/sdlc/states/gate/transitions/run/executor/definitionId")
        .and_then(Value::as_str)
        .expect("executor resolved")
        .to_string()
}

#[test]
fn project_beats_framework_beats_language_beats_generic() {
    let cap = resolved_cap(config_with_stack(
        json!({ "language": "rust", "primary_framework": "axum", "project": "myapp" }),
        &[
            "cap.verify.myapp",
            "cap.verify.axum",
            "cap.verify.rust",
            "cap.verify.generic",
        ],
    ));
    assert_eq!(
        cap, "cap.verify.myapp",
        "project is the most specific level"
    );
}

#[test]
fn framework_beats_language() {
    let cap = resolved_cap(config_with_stack(
        json!({ "language": "rust", "primary_framework": "axum" }),
        &["cap.verify.axum", "cap.verify.rust", "cap.verify.generic"],
    ));
    assert_eq!(cap, "cap.verify.axum", "primary_framework beats language");
}

#[test]
fn language_beats_generic() {
    let cap = resolved_cap(config_with_stack(
        json!({ "language": "rust" }),
        &["cap.verify.rust", "cap.verify.generic"],
    ));
    assert_eq!(cap, "cap.verify.rust", "language beats the generic floor");
}

#[test]
fn absent_levels_are_skipped() {
    // project + framework declared, but only the language cap (and generic)
    // exist — resolution skips the missing project/framework levels.
    let cap = resolved_cap(config_with_stack(
        json!({ "language": "rust", "primary_framework": "axum", "project": "myapp" }),
        &["cap.verify.rust", "cap.verify.generic"],
    ));
    assert_eq!(
        cap, "cap.verify.rust",
        "missing levels fall through to language"
    );
}

#[test]
fn framework_present_language_absent_falls_to_generic() {
    // Only generic exists; the framework/language caps are absent.
    let cap = resolved_cap(config_with_stack(
        json!({ "language": "rust", "primary_framework": "axum" }),
        &["cap.verify.generic"],
    ));
    assert_eq!(
        cap, "cap.verify.generic",
        "no specific level → generic floor"
    );
}

#[test]
fn plain_string_stack_still_resolves() {
    // Back-compat: a bare string is language-only.
    let cap = resolved_cap(config_with_stack(
        json!("rust"),
        &["cap.verify.rust", "cap.verify.generic"],
    ));
    assert_eq!(
        cap, "cap.verify.rust",
        "plain-string stack resolves as language"
    );
}

#[test]
fn additive_frameworks_field_is_accepted_but_only_primary_resolves() {
    // `frameworks: [set]` is accepted (additive-knowledge composition is a
    // separate later concern); only `primary_framework` participates in
    // override-resolution.
    let cap = resolved_cap(config_with_stack(
        json!({
            "language": "rust",
            "frameworks": ["axum", "tokio"],
            "primary_framework": "axum"
        }),
        &["cap.verify.axum", "cap.verify.rust", "cap.verify.generic"],
    ));
    assert_eq!(cap, "cap.verify.axum", "only primary_framework overrides");
}
