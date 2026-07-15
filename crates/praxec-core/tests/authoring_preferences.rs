//! SPEC §17.x (v0.3) — `praxec.authoring.*` advisory preferences.
//! Surfaced to LLM-driven authoring workflows via template substitution
//! (`{{$.praxec.authoring.*}}`). FMECA atomic assertions for shape
//! validation, snapshot stamping, and template resolution.
//!
//! These preferences are intentionally **advisory** — no runtime branch
//! reads them, no validator rejects a workflow for ignoring them. The
//! mechanism is a template-resolvable signal an authoring skill can
//! include in its body so the LLM sees the operator's preference.

use praxec_core::config::resolve_str;

// ── shape validation at config load ────────────────────────────────────────

#[test]
fn preferred_script_language_string_accepted() {
    let yaml = r#"
version: "1.0.0"
praxec:
  authoring:
    preferred_script_language: bash
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    resolve_str(yaml).expect("valid preference must load");
}

#[test]
fn empty_preferred_script_language_rejects_with_invalid_authoring_preference() {
    let yaml = r#"
version: "1.0.0"
praxec:
  authoring:
    preferred_script_language: ""
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    let err = resolve_str(yaml).expect_err("empty preference must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_AUTHORING_PREFERENCE"), "got: {s}");
    assert!(
        s.contains("preferred_script_language"),
        "error must name the offending field; got: {s}"
    );
}

#[test]
fn non_string_preferred_script_language_rejects() {
    let yaml = r#"
version: "1.0.0"
praxec:
  authoring:
    preferred_script_language: 42
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    let err = resolve_str(yaml).expect_err("numeric preference must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_AUTHORING_PREFERENCE"), "got: {s}");
    assert!(
        s.contains("number"),
        "error must name the wrong shape; got: {s}"
    );
}

#[test]
fn non_object_authoring_block_rejects() {
    let yaml = r#"
version: "1.0.0"
praxec:
  authoring: "bash"
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    let err = resolve_str(yaml).expect_err("scalar authoring block must reject");
    let s = format!("{err:?}");
    assert!(s.contains("INVALID_AUTHORING_PREFERENCE"), "got: {s}");
    assert!(
        s.contains("must be an object"),
        "error must explain the shape; got: {s}"
    );
}

#[test]
fn absent_authoring_block_loads_cleanly() {
    let yaml = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    resolve_str(yaml).expect("missing authoring block is fine — preferences are optional");
}

// ── snapshot stamping ──────────────────────────────────────────────────────

#[test]
fn preferences_stamp_into_workflow_snapshot_as_authoring_prefs() {
    let yaml = r#"
version: "1.0.0"
praxec:
  authoring:
    preferred_script_language: python3
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("loads");
    let prefs = resolved
        .pointer("/workflows/demo/_authoringPrefs")
        .expect("every workflow must get _authoringPrefs when authoring block is present");
    assert_eq!(
        prefs["preferred_script_language"], "python3",
        "snapshot must carry the operator's preference; got: {prefs}"
    );
}

#[test]
fn no_authoring_prefs_stamped_when_authoring_block_absent() {
    let yaml = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: s
    states:
      s: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("loads");
    assert!(
        resolved
            .pointer("/workflows/demo/_authoringPrefs")
            .is_none(),
        "no stamping when authoring is absent — snapshot stays unbloated"
    );
}

#[test]
fn all_workflows_share_the_same_preferences_snapshot() {
    // Per SPEC §8.4 the `praxec.*` block is gateway-wide; per-workflow
    // override is rejected with CONFIG_FLAG_NOT_RUNTIME_MUTABLE. So every
    // workflow's _authoringPrefs is identical at config-resolve time.
    let yaml = r#"
version: "1.0.0"
praxec:
  authoring:
    preferred_script_language: powershell
workflows:
  a:
    initialState: s
    states:
      s: { terminal: true }
  b:
    initialState: s
    states:
      s: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("loads");
    let a = resolved.pointer("/workflows/a/_authoringPrefs").unwrap();
    let b = resolved.pointer("/workflows/b/_authoringPrefs").unwrap();
    assert_eq!(a, b, "all workflow snapshots must carry identical prefs");
}

// ── template substitution ─────────────────────────────────────────────────

#[tokio::test]
async fn template_substitutes_preferred_script_language_into_skill_guidance() {
    use praxec_core::WorkflowRuntime;
    use praxec_core::audit::{AuditSink, NullAuditSink};
    use praxec_core::guards::DefaultGuardEvaluator;
    use praxec_core::model::{Principal, StartWorkflow};
    use praxec_core::ports::ExecutorRegistry;
    use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use std::sync::Arc;

    struct NoopRegistry;
    impl ExecutorRegistry for NoopRegistry {
        fn get(&self, _: &str) -> Option<Arc<dyn praxec_core::Executor>> {
            None
        }
    }

    let yaml = r#"
version: "1.0.0"
praxec:
  authoring:
    preferred_script_language: bash
workflows:
  demo:
    initialState: writing
    states:
      writing:
        goal: "Write a new script in {{$.praxec.authoring.preferred_script_language}}"
        guidance: "Default to {{$.praxec.authoring.preferred_script_language}} unless a cross-platform need says otherwise."
        transitions:
          done:
            target: terminal
            executor: { kind: noop }
      terminal: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("loads");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        defs,
        store,
        Arc::new(NoopRegistry),
        guards,
        Arc::new(NullAuditSink) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: serde_json::json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start");
    let goal = resp
        .pointer("/guidance/goal")
        .and_then(|v| v.as_str())
        .unwrap();
    let instructions = resp
        .pointer("/guidance/instructions")
        .and_then(|v| v.as_str())
        .unwrap();
    assert_eq!(
        goal, "Write a new script in bash",
        "goal template must substitute the preference; got: {goal}"
    );
    assert!(
        instructions.contains("Default to bash"),
        "instructions template must substitute the preference; got: {instructions}"
    );
}

#[tokio::test]
async fn missing_preference_renders_as_unset_stub_not_panic() {
    // No praxec.authoring block declared, but skill body still references
    // the template. The resolver MUST emit a stub, not panic, not strip the
    // placeholder silently.
    use praxec_core::WorkflowRuntime;
    use praxec_core::audit::{AuditSink, NullAuditSink};
    use praxec_core::guards::DefaultGuardEvaluator;
    use praxec_core::model::{Principal, StartWorkflow};
    use praxec_core::ports::ExecutorRegistry;
    use praxec_core::store::{ConfigDefinitionStore, InMemoryWorkflowStore};
    use std::sync::Arc;

    struct NoopRegistry;
    impl ExecutorRegistry for NoopRegistry {
        fn get(&self, _: &str) -> Option<Arc<dyn praxec_core::Executor>> {
            None
        }
    }

    let yaml = r#"
version: "1.0.0"
workflows:
  demo:
    initialState: writing
    states:
      writing:
        goal: "Lang: {{$.praxec.authoring.preferred_script_language}}"
        transitions:
          done:
            target: terminal
            executor: { kind: noop }
      terminal: { terminal: true }
"#;
    let resolved = resolve_str(yaml).expect("loads");
    let defs = Arc::new(ConfigDefinitionStore::from_config(&resolved));
    let store = Arc::new(InMemoryWorkflowStore::new());
    let guards = Arc::new(DefaultGuardEvaluator::new());
    let runtime = WorkflowRuntime::new(
        defs,
        store,
        Arc::new(NoopRegistry),
        guards,
        Arc::new(NullAuditSink) as Arc<dyn AuditSink>,
    )
    .with_writable_repo_roots(vec![praxec_core::RepoRoot::for_test()]);
    let resp = runtime
        .start(StartWorkflow {
            definition_id: "demo".into(),
            input: serde_json::json!({}),
            principal: Principal::anonymous(),
            run_env: praxec_core::RunEnv::for_test(),
            depth: 0,
            parent: None,
        })
        .await
        .expect("start");
    let goal = resp
        .pointer("/guidance/goal")
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(
        goal.contains("(preferred_script_language: unset)") || goal.contains("(unset)"),
        "missing preference must render as an `unset` stub, not panic or silently strip; got: {goal}"
    );
}
